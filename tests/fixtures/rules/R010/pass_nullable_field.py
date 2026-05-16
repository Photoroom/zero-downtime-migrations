# R010: AddField with null=True - should pass
from django.db import migrations, models


class Migration(migrations.Migration):

    dependencies = [
        ('myapp', '0007'),
    ]

    operations = [
        migrations.AddField(
            model_name='order',
            name='note',
            field=models.CharField(max_length=100, null=True),
        ),
    ]
