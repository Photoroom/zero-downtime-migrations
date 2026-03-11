# R015: AlterField to NOT NULL - should fail
from django.db import migrations, models


class Migration(migrations.Migration):

    dependencies = [
        ('myapp', '0010'),
    ]

    operations = [
        migrations.AlterField(
            model_name='order',
            name='status',
            field=models.CharField(max_length=50, null=False),
        ),
    ]
